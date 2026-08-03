//! Selection-aware reductions over typed table columns.
//!
//! This module provides storage-level `COUNT` and `SUM` primitives. SQL
//! aggregate parsing, grouping, and nullable aggregate semantics belong to a
//! later query-execution layer.

use crate::scan::RowSelection;
use crate::storage::Column;
use crate::{DataType, Table, Value};
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
    /// `SUM` was requested for a nonnumeric column.
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

        let column_index = self
            .fields()
            .iter()
            .position(|candidate| candidate.name() == field)
            .ok_or_else(|| ReductionError::FieldNotFound {
                name: field.to_owned(),
            })?;

        match &self.columns()[column_index] {
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
