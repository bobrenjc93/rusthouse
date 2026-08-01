use std::cmp::Ordering;

use crate::batch::{Bitmap, Column, RecordBatch, SelectionMask};
use crate::error::{Error, Result};
use crate::value::compare_int_float;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOp {
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
}

impl ComparisonOp {
    fn evaluate<T: PartialOrd>(self, left: &T, right: &T) -> bool {
        match self {
            Self::Eq => left == right,
            Self::NotEq => left != right,
            Self::Less => left < right,
            Self::LessEq => left <= right,
            Self::Greater => left > right,
            Self::GreaterEq => left >= right,
        }
    }

    fn evaluate_ordering(self, ordering: Ordering) -> bool {
        self.evaluate_optional_ordering(Some(ordering))
    }

    fn evaluate_optional_ordering(self, ordering: Option<Ordering>) -> bool {
        match self {
            Self::Eq => ordering == Some(Ordering::Equal),
            Self::NotEq => ordering != Some(Ordering::Equal),
            Self::Less => ordering == Some(Ordering::Less),
            Self::LessEq => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
            Self::Greater => ordering == Some(Ordering::Greater),
            Self::GreaterEq => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
        }
    }
}

fn selected_predicate(
    batch: &RecordBatch,
    validity: &Bitmap,
    mut predicate: impl FnMut(usize) -> bool,
) -> Result<SelectionMask> {
    let mut result = SelectionMask::none(batch.len(), batch.capacity())?;
    for word_index in 0..batch.selection().word_count() {
        let mut candidates = batch.selection().word(word_index) & validity.word(word_index);
        let mut matched = 0_u64;
        while candidates != 0 {
            let bit = candidates.trailing_zeros() as usize;
            let row = word_index * u64::BITS as usize + bit;
            if predicate(row) {
                matched |= 1_u64 << bit;
            }
            candidates &= candidates - 1;
        }
        result.set_word(word_index, matched);
    }
    Ok(result)
}

pub fn compare_i64(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: i64,
) -> Result<SelectionMask> {
    let array = match batch.column(column)? {
        Column::Int64(array) => array,
        actual => {
            return Err(Error::BatchTypeMismatch {
                column,
                expected: "Int64",
                actual: actual.data_type().name(),
            });
        }
    };
    selected_predicate(batch, array.validity(), |row| {
        op.evaluate(&array.values()[row], &value)
    })
}

pub fn compare_f64(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: f64,
) -> Result<SelectionMask> {
    let array = match batch.column(column)? {
        Column::Float64(array) => array,
        actual => {
            return Err(Error::BatchTypeMismatch {
                column,
                expected: "Float64",
                actual: actual.data_type().name(),
            });
        }
    };
    selected_predicate(batch, array.validity(), |row| {
        op.evaluate(&array.values()[row], &value)
    })
}

pub fn compare_i64_f64(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: f64,
) -> Result<SelectionMask> {
    let array = match batch.column(column)? {
        Column::Int64(array) => array,
        actual => {
            return Err(Error::BatchTypeMismatch {
                column,
                expected: "Int64",
                actual: actual.data_type().name(),
            });
        }
    };
    selected_predicate(batch, array.validity(), |row| {
        op.evaluate_optional_ordering(compare_int_float(array.values()[row], value))
    })
}

pub fn compare_f64_i64(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: i64,
) -> Result<SelectionMask> {
    let array = match batch.column(column)? {
        Column::Float64(array) => array,
        actual => {
            return Err(Error::BatchTypeMismatch {
                column,
                expected: "Float64",
                actual: actual.data_type().name(),
            });
        }
    };
    selected_predicate(batch, array.validity(), |row| {
        op.evaluate_optional_ordering(
            compare_int_float(value, array.values()[row]).map(Ordering::reverse),
        )
    })
}

pub fn compare_bool(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: bool,
) -> Result<SelectionMask> {
    let array = match batch.column(column)? {
        Column::Boolean(array) => array,
        actual => {
            return Err(Error::BatchTypeMismatch {
                column,
                expected: "Boolean",
                actual: actual.data_type().name(),
            });
        }
    };
    selected_predicate(batch, array.validity(), |row| {
        op.evaluate(&array.value(row).expect("candidate is valid"), &value)
    })
}

pub fn compare_string(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: &str,
) -> Result<SelectionMask> {
    compare_string_controlled(batch, column, op, value, || Ok(()))
}

pub fn compare_string_controlled(
    batch: &RecordBatch,
    column: usize,
    op: ComparisonOp,
    value: &str,
    mut check_cancellation: impl FnMut() -> Result<()>,
) -> Result<SelectionMask> {
    let array = match batch.column(column)? {
        Column::String(array) => array,
        actual => {
            return Err(Error::BatchTypeMismatch {
                column,
                expected: "String",
                actual: actual.data_type().name(),
            });
        }
    };
    let mut dictionary_matches = vec![None; array.dictionary().len()];
    let mut result = SelectionMask::none(batch.len(), batch.capacity())?;
    for word_index in 0..batch.selection().word_count() {
        let mut candidates = batch.selection().word(word_index) & array.validity().word(word_index);
        let mut matched = 0_u64;
        while candidates != 0 {
            check_cancellation()?;
            let bit = candidates.trailing_zeros() as usize;
            let row = word_index * u64::BITS as usize + bit;
            let key = array.key(row).expect("candidate is valid") as usize;
            let is_match = match dictionary_matches[key] {
                Some(is_match) => is_match,
                None => {
                    let is_match = op.evaluate_ordering(compare_strings_controlled(
                        array.value(row).expect("candidate is valid"),
                        value,
                        &mut check_cancellation,
                    )?);
                    dictionary_matches[key] = Some(is_match);
                    is_match
                }
            };
            if is_match {
                matched |= 1_u64 << bit;
            }
            candidates &= candidates - 1;
        }
        result.set_word(word_index, matched);
    }
    Ok(result)
}

pub(crate) fn compare_string_values<'a>(
    selection: &SelectionMask,
    op: ComparisonOp,
    value: &str,
    mut value_at: impl FnMut(usize) -> Option<&'a str>,
    mut check_cancellation: impl FnMut() -> Result<()>,
) -> Result<SelectionMask> {
    let mut result = SelectionMask::none(selection.len(), selection.capacity())?;
    for word_index in 0..selection.word_count() {
        let mut candidates = selection.word(word_index);
        let mut matched = 0_u64;
        while candidates != 0 {
            check_cancellation()?;
            let bit = candidates.trailing_zeros() as usize;
            let row = word_index * u64::BITS as usize + bit;
            if let Some(candidate) = value_at(row)
                && op.evaluate_ordering(compare_strings_controlled(
                    candidate,
                    value,
                    &mut check_cancellation,
                )?)
            {
                matched |= 1_u64 << bit;
            }
            candidates &= candidates - 1;
        }
        result.set_word(word_index, matched);
    }
    Ok(result)
}

const STRING_COMPARISON_CHUNK_BYTES: usize = 64 * 1024;

fn compare_strings_controlled(
    left: &str,
    right: &str,
    check_cancellation: &mut impl FnMut() -> Result<()>,
) -> Result<Ordering> {
    let common_len = left.len().min(right.len());
    let mut offset = 0;
    while offset < common_len {
        check_cancellation()?;
        let end = (offset + STRING_COMPARISON_CHUNK_BYTES).min(common_len);
        let ordering = left.as_bytes()[offset..end].cmp(&right.as_bytes()[offset..end]);
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
        offset = end;
    }
    Ok(left.len().cmp(&right.len()))
}

pub fn compare_columns(
    batch: &RecordBatch,
    left: usize,
    op: ComparisonOp,
    right: usize,
) -> Result<SelectionMask> {
    let left_column = batch.column(left)?;
    let right_column = batch.column(right)?;
    if left_column.data_type() != right_column.data_type() {
        return Err(Error::BatchTypeMismatch {
            column: right,
            expected: left_column.data_type().name(),
            actual: right_column.data_type().name(),
        });
    }

    let mut validity = left_column.validity().clone();
    validity.intersect_with(right_column.validity());
    match (left_column, right_column) {
        (Column::Int64(left), Column::Int64(right)) => {
            selected_predicate(batch, &validity, |row| {
                op.evaluate(&left.values()[row], &right.values()[row])
            })
        }
        (Column::Float64(left), Column::Float64(right)) => {
            selected_predicate(batch, &validity, |row| {
                op.evaluate(&left.values()[row], &right.values()[row])
            })
        }
        (Column::Boolean(left), Column::Boolean(right)) => {
            selected_predicate(batch, &validity, |row| {
                op.evaluate(
                    &left.value(row).expect("candidate is valid"),
                    &right.value(row).expect("candidate is valid"),
                )
            })
        }
        (Column::String(left), Column::String(right)) => {
            selected_predicate(batch, &validity, |row| {
                op.evaluate_ordering(
                    left.value(row)
                        .expect("candidate is valid")
                        .cmp(right.value(row).expect("candidate is valid")),
                )
            })
        }
        _ => unreachable!("matching data types were checked above"),
    }
}

pub fn is_null(batch: &RecordBatch, column: usize) -> Result<SelectionMask> {
    let validity = batch.column(column)?.validity();
    let mut result = SelectionMask::none(batch.len(), batch.capacity())?;
    for word in 0..result.word_count() {
        result.set_word(word, batch.selection().word(word) & !validity.word(word));
    }
    Ok(result)
}

pub fn is_not_null(batch: &RecordBatch, column: usize) -> Result<SelectionMask> {
    let validity = batch.column(column)?.validity();
    let mut result = SelectionMask::none(batch.len(), batch.capacity())?;
    for word in 0..result.word_count() {
        result.set_word(word, batch.selection().word(word) & validity.word(word));
    }
    Ok(result)
}
