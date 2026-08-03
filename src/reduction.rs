//! Selection-aware reductions over typed table columns.
//!
//! These are storage-level aggregate primitives. SQL aggregate parsing,
//! grouping, and nullable aggregate semantics belong to a later
//! query-execution layer.

use crate::scan::RowSelection;
use crate::storage::Column;
use crate::{DataType, Table, Value};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// A validation or arithmetic error from a table reduction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReductionError {
    /// A row selection represents a different number of rows than the table.
    SelectionLengthMismatch {
        /// Number of rows in the table being reduced.
        table_rows: usize,
        /// Number of rows represented by the supplied selection.
        selection_rows: usize,
    },
    /// The requested field does not exist in the table schema.
    FieldNotFound {
        /// The requested, case-sensitive field name.
        name: String,
    },
    /// A numeric reduction was requested for a nonnumeric column.
    NonNumericColumn {
        /// Name of the requested field.
        field: String,
        /// Physical type declared by the column.
        data_type: DataType,
    },
    /// Adding an `Int64` value would exceed the `Int64` range.
    Int64Overflow {
        /// Name of the field being summed.
        field: String,
        /// Zero-based table row whose value caused the overflow.
        row: usize,
    },
}

impl fmt::Display for ReductionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectionLengthMismatch {
                table_rows,
                selection_rows,
            } => write!(
                formatter,
                "selection represents {selection_rows} rows; table contains {table_rows} rows"
            ),
            Self::FieldNotFound { name } => write!(formatter, "field `{name}` does not exist"),
            Self::NonNumericColumn { field, data_type } => {
                write!(formatter, "field `{field}` has nonnumeric type {data_type}")
            }
            Self::Int64Overflow { field, row } => {
                write!(
                    formatter,
                    "summing Int64 field `{field}` overflowed at row {row}"
                )
            }
        }
    }
}

impl Error for ReductionError {}

impl Table {
    /// Counts all table rows or only the rows in `selection`.
    ///
    /// A supplied selection must represent exactly [`Table::len`] rows. An
    /// empty table or a selection with no set bits returns zero.
    pub fn count(&self, selection: Option<&RowSelection>) -> Result<usize, ReductionError> {
        validate_selection(self.len(), selection)?;
        Ok(selection.map_or_else(|| self.len(), RowSelection::selected_count))
    }

    /// Sums an `Int64` or `Float64` column over all or selected rows.
    ///
    /// Empty inputs produce the zero value of the column's physical type.
    /// `Int64` values are added in ascending row order with checked arithmetic;
    /// the first overflowing addition returns [`ReductionError::Int64Overflow`].
    /// `Float64` values are added in ascending row order starting from `+0.0`
    /// using native IEEE 754 addition. Consequently NaN and infinities
    /// propagate according to IEEE 754, and finite overflow produces a signed
    /// infinity rather than a reduction error.
    pub fn sum(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
    ) -> Result<Value, ReductionError> {
        validate_selection(self.len(), selection)?;

        let column = self.reduction_column(field)?;
        match column {
            Column::Int64(values) => match selection {
                Some(selection) => sum_int64(field, values, selection.selected_rows()),
                None => sum_int64(field, values, 0..values.len()),
            },
            Column::Float64(values) => {
                let total = match selection {
                    Some(selection) => sum_float64(values, selection.selected_rows()),
                    None => sum_float64(values, 0..values.len()),
                };
                Ok(Value::Float64(total))
            }
            column => Err(ReductionError::NonNumericColumn {
                field: field.to_owned(),
                data_type: column.data_type(),
            }),
        }
    }

    /// Averages an `Int64` or `Float64` column over all or selected rows.
    ///
    /// Both input types produce a [`Value::Float64`]. Empty tables and
    /// selections with no set bits return `None`. A supplied selection must
    /// represent exactly [`Table::len`] rows.
    ///
    /// `Int64` values are summed exactly in an `i128` accumulator and the
    /// resulting rational number is rounded once to the nearest `f64`, with
    /// ties resolved to an even significand. `Float64` values are added in
    /// ascending row order starting from `+0.0`. Ordered count-scaled terms
    /// provide an overflow-resistant result if that total overflows for finite
    /// inputs, with a running mean as a finite fallback. NaN and infinities
    /// propagate according to IEEE 754, and an input containing only signed
    /// zeroes averages to `+0.0`.
    pub fn avg(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
    ) -> Result<Option<Value>, ReductionError> {
        validate_selection(self.len(), selection)?;

        let column = self.reduction_column(field)?;
        match column {
            Column::Int64(values) => Ok(match selection {
                Some(selection) => avg_int64(values, selection.selected_rows()),
                None => avg_int64(values, 0..values.len()),
            }),
            Column::Float64(values) => Ok(match selection {
                Some(selection) => avg_float64(
                    values,
                    selection.selected_rows(),
                    selection.selected_count(),
                ),
                None => avg_float64(values, 0..values.len(), values.len()),
            }),
            column => Err(ReductionError::NonNumericColumn {
                field: field.to_owned(),
                data_type: column.data_type(),
            }),
        }
    }

    /// Finds the minimum value of a column over all or selected rows.
    ///
    /// Every physical column type is supported. Integers use signed ordering,
    /// Booleans use `false < true`, and strings use lexicographic Unicode scalar
    /// value ordering. `Float64` values use [`f64::total_cmp`], including NaNs:
    /// `-0.0` sorts below `+0.0`, and NaN sign, payload, and signaling state
    /// participate in a deterministic total order. Empty tables and selections
    /// with no set bits return `None`.
    pub fn min(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
    ) -> Result<Option<Value>, ReductionError> {
        self.extreme(field, selection, Extreme::Minimum)
    }

    /// Finds the maximum value of a column over all or selected rows.
    ///
    /// Ordering and empty-input behavior are identical to [`Table::min`]. In
    /// particular, `+0.0` sorts above `-0.0`, and every NaN bit pattern has a
    /// deterministic position in the [`f64::total_cmp`] order.
    pub fn max(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
    ) -> Result<Option<Value>, ReductionError> {
        self.extreme(field, selection, Extreme::Maximum)
    }

    fn extreme(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
        extreme: Extreme,
    ) -> Result<Option<Value>, ReductionError> {
        validate_selection(self.len(), selection)?;
        let column = self.reduction_column(field)?;
        Ok(match selection {
            Some(selection) => reduce_extreme(column, selection.selected_rows(), extreme),
            None => reduce_extreme(column, 0..self.len(), extreme),
        })
    }

    fn reduction_column(&self, field: &str) -> Result<&Column, ReductionError> {
        let column_index = self
            .fields()
            .iter()
            .position(|candidate| candidate.name() == field)
            .ok_or_else(|| ReductionError::FieldNotFound {
                name: field.to_owned(),
            })?;
        Ok(&self.columns()[column_index])
    }
}

#[derive(Clone, Copy)]
enum Extreme {
    Minimum,
    Maximum,
}

impl Extreme {
    fn prefers(self, ordering: Ordering) -> bool {
        match self {
            Self::Minimum => ordering.is_lt(),
            Self::Maximum => ordering.is_gt(),
        }
    }
}

fn validate_selection(
    table_rows: usize,
    selection: Option<&RowSelection>,
) -> Result<(), ReductionError> {
    if let Some(selection) = selection
        && selection.len() != table_rows
    {
        return Err(ReductionError::SelectionLengthMismatch {
            table_rows,
            selection_rows: selection.len(),
        });
    }
    Ok(())
}

fn sum_int64(
    field: &str,
    values: &[i64],
    rows: impl Iterator<Item = usize>,
) -> Result<Value, ReductionError> {
    let mut total = 0_i64;
    for row in rows {
        total = total
            .checked_add(values[row])
            .ok_or_else(|| ReductionError::Int64Overflow {
                field: field.to_owned(),
                row,
            })?;
    }
    Ok(Value::Int64(total))
}

fn sum_float64(values: &[f64], rows: impl Iterator<Item = usize>) -> f64 {
    let mut total = 0.0_f64;
    for row in rows {
        total += values[row];
    }
    total
}

fn avg_int64(values: &[i64], rows: impl Iterator<Item = usize>) -> Option<Value> {
    let mut total = 0_i128;
    let mut count = 0_usize;
    for row in rows {
        total += i128::from(values[row]);
        count += 1;
    }
    (count != 0).then(|| Value::Float64(int_ratio_to_f64(total, count)))
}

fn int_ratio_to_f64(numerator: i128, denominator: usize) -> f64 {
    debug_assert_ne!(denominator, 0);
    if numerator == 0 {
        return 0.0;
    }

    const SIGNIFICAND_BITS: i32 = 52;
    const IMPLICIT_BIT: u128 = 1_u128 << SIGNIFICAND_BITS;
    const SIGN_BIT: u64 = 1_u64 << 63;
    const EXPONENT_BIAS: i32 = 1023;

    let negative = numerator.is_negative();
    let numerator = numerator.unsigned_abs();
    let denominator = denominator as u128;
    let mut exponent = floor_log2(numerator) - floor_log2(denominator);
    let below_exponent = if exponent >= 0 {
        numerator < denominator << exponent as u32
    } else {
        numerator << exponent.unsigned_abs() < denominator
    };
    if below_exponent {
        exponent -= 1;
    }

    // Normalize the exact ratio to 53 significant bits, retaining its
    // remainder so the final conversion performs one ties-to-even rounding.
    let shift = SIGNIFICAND_BITS - exponent;
    let (mut significand, remainder, divisor) = if shift >= 0 {
        let scaled_numerator = numerator << shift as u32;
        (
            scaled_numerator / denominator,
            scaled_numerator % denominator,
            denominator,
        )
    } else {
        let scaled_denominator = denominator << shift.unsigned_abs();
        (
            numerator / scaled_denominator,
            numerator % scaled_denominator,
            scaled_denominator,
        )
    };

    let distance_to_upper = divisor - remainder;
    if remainder > distance_to_upper || (remainder == distance_to_upper && significand & 1 != 0) {
        significand += 1;
    }
    if significand == IMPLICIT_BIT << 1 {
        significand >>= 1;
        exponent += 1;
    }

    debug_assert!((IMPLICIT_BIT..IMPLICIT_BIT << 1).contains(&significand));
    let sign = if negative { SIGN_BIT } else { 0 };
    let biased_exponent = u64::try_from(exponent + EXPONENT_BIAS)
        .expect("an Int64 average always has a normal Float64 exponent");
    let fraction =
        u64::try_from(significand - IMPLICIT_BIT).expect("a Float64 fraction contains 52 bits");
    f64::from_bits(sign | biased_exponent << SIGNIFICAND_BITS | fraction)
}

fn floor_log2(value: u128) -> i32 {
    i32::try_from(u128::BITS - 1 - value.leading_zeros()).expect("u128 logarithms fit in i32")
}

fn avg_float64(values: &[f64], rows: impl Iterator<Item = usize>, count: usize) -> Option<Value> {
    if count == 0 {
        return None;
    }

    let mut total = 0.0_f64;
    let mut scaled_total = 0.0_f64;
    let mut running_mean = 0.0_f64;
    let mut seen = 0_usize;
    let mut all_finite = true;
    let divisor = count as f64;
    for row in rows {
        let value = values[row];
        total += value;
        scaled_total += value / divisor;
        seen += 1;
        if all_finite && value.is_finite() {
            running_mean = update_float_mean(running_mean, value, seen);
        } else {
            all_finite = false;
        }
    }
    debug_assert_eq!(seen, count);
    let average = if all_finite && total.is_infinite() {
        if scaled_total.is_finite() {
            scaled_total
        } else {
            running_mean
        }
    } else {
        total / divisor
    };
    Some(Value::Float64(average))
}

fn update_float_mean(mean: f64, value: f64, count: usize) -> f64 {
    let count = count as f64;
    if mean.is_sign_negative() == value.is_sign_negative() {
        mean + (value - mean) / count
    } else {
        // Subtracting opposite-sign extremes can overflow before division.
        mean * ((count - 1.0) / count) + value / count
    }
}

fn reduce_extreme(
    column: &Column,
    rows: impl Iterator<Item = usize>,
    extreme: Extreme,
) -> Option<Value> {
    match column {
        Column::Int64(values) => {
            extreme_index(values, rows, i64::cmp, extreme).map(|row| Value::Int64(values[row]))
        }
        Column::Float64(values) => extreme_index(values, rows, f64::total_cmp, extreme)
            .map(|row| Value::Float64(values[row])),
        Column::Bool(values) => {
            extreme_index(values, rows, u8::cmp, extreme).map(|row| Value::Bool(values[row] != 0))
        }
        Column::String(values) => extreme_index(values, rows, String::cmp, extreme)
            .map(|row| Value::String(values[row].clone())),
    }
}

fn extreme_index<T>(
    values: &[T],
    mut rows: impl Iterator<Item = usize>,
    compare: impl Fn(&T, &T) -> Ordering,
    extreme: Extreme,
) -> Option<usize> {
    let mut result = rows.next()?;
    for row in rows {
        if extreme.prefers(compare(&values[row], &values[result])) {
            result = row;
        }
    }
    Some(result)
}
