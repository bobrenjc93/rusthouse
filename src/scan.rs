//! Typed comparison scans and compact row selections.
//!
//! This module deliberately covers one predicate primitive: comparing every
//! value in one physical column with a same-typed literal. SQL parsing and
//! predicate composition belong to later query-engine layers.

use crate::{DataType, Table, Value};
use std::error::Error;
use std::fmt;

const BITS_PER_BYTE: usize = u8::BITS as usize;

/// A comparison applied by [`Table::scan`] to each value in a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonOperator {
    /// Values compare equal (`=`).
    Equal,
    /// Values do not compare equal (`!=`).
    NotEqual,
    /// Column values are less than the literal (`<`).
    LessThan,
    /// Column values are less than or equal to the literal (`<=`).
    LessThanOrEqual,
    /// Column values are greater than the literal (`>`).
    GreaterThan,
    /// Column values are greater than or equal to the literal (`>=`).
    GreaterThanOrEqual,
}

impl ComparisonOperator {
    fn compare<T: PartialEq + PartialOrd>(self, column_value: &T, literal: &T) -> bool {
        match self {
            Self::Equal => column_value == literal,
            Self::NotEqual => column_value != literal,
            Self::LessThan => column_value < literal,
            Self::LessThanOrEqual => column_value <= literal,
            Self::GreaterThan => column_value > literal,
            Self::GreaterThanOrEqual => column_value >= literal,
        }
    }
}

/// An allocation failure while constructing a [`RowSelection`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionAllocationError {
    row_count: usize,
    required_bytes: usize,
}

impl SelectionAllocationError {
    /// Returns the number of rows the requested bitmap would represent.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the number of bitmap bytes that could not be allocated.
    #[must_use]
    pub const fn required_bytes(&self) -> usize {
        self.required_bytes
    }
}

impl fmt::Display for SelectionAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not allocate {} bitmap bytes for {} rows",
            self.required_bytes, self.row_count
        )
    }
}

impl Error for SelectionAllocationError {}

/// A compact bitmap identifying selected zero-based table rows.
///
/// The bitmap uses one bit per row and leaves unused bits in its final byte
/// clear. Construction is fallible so callers can handle capacity and address
/// space limits without an allocation panic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSelection {
    bytes: Vec<u8>,
    row_count: usize,
}

impl RowSelection {
    /// Creates a selection with `row_count` clear bits.
    ///
    /// The allocation is bounded to exactly `ceil(row_count / 8)` initialized
    /// bytes. A zero-row selection performs no allocation.
    pub fn try_empty(row_count: usize) -> Result<Self, SelectionAllocationError> {
        let required_bytes = bitmap_byte_len(row_count);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(required_bytes)
            .map_err(|_| SelectionAllocationError {
                row_count,
                required_bytes,
            })?;
        bytes.resize(required_bytes, 0);
        Ok(Self { bytes, row_count })
    }

    /// Returns the number of represented rows.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.row_count
    }

    /// Returns whether this selection represents no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the initialized storage used by the packed bitmap.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns whether `row` is selected, or `None` when it is out of bounds.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<bool> {
        if row >= self.row_count {
            return None;
        }
        let byte = self.bytes[row / BITS_PER_BYTE];
        let mask = 1_u8 << (row % BITS_PER_BYTE);
        Some(byte & mask != 0)
    }

    /// Iterates over one Boolean selection value per represented row.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = bool> + DoubleEndedIterator + '_ {
        (0..self.row_count).map(|row| {
            self.get(row)
                .expect("iterator only visits rows inside the selection")
        })
    }

    /// Iterates over the zero-based indexes of selected rows.
    pub fn selected_rows(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        (0..self.row_count).filter(|row| {
            self.get(*row)
                .expect("iterator only visits rows inside the selection")
        })
    }

    /// Returns the number of selected rows.
    #[must_use]
    pub fn selected_count(&self) -> usize {
        self.bytes
            .iter()
            .map(|byte| byte.count_ones() as usize)
            .sum()
    }

    fn select(&mut self, row: usize) {
        debug_assert!(row < self.row_count);
        self.bytes[row / BITS_PER_BYTE] |= 1_u8 << (row % BITS_PER_BYTE);
    }
}

/// A validation or allocation error from [`Table::scan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The requested field does not exist in the table schema.
    FieldNotFound {
        /// The requested, case-sensitive field name.
        name: String,
    },
    /// The comparison literal does not have the column's physical type.
    TypeMismatch {
        /// Name of the compared field.
        field: String,
        /// Physical type declared by the column.
        column_type: DataType,
        /// Physical type of the supplied literal.
        literal_type: DataType,
    },
    /// The row-selection bitmap could not be allocated.
    AllocationFailed(SelectionAllocationError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldNotFound { name } => write!(formatter, "field `{name}` does not exist"),
            Self::TypeMismatch {
                field,
                column_type,
                literal_type,
            } => write!(
                formatter,
                "field `{field}` has type {column_type}; comparison literal has type {literal_type}"
            ),
            Self::AllocationFailed(error) => error.fmt(formatter),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AllocationFailed(error) => Some(error),
            Self::FieldNotFound { .. } | Self::TypeMismatch { .. } => None,
        }
    }
}

impl From<SelectionAllocationError> for ScanError {
    fn from(error: SelectionAllocationError) -> Self {
        Self::AllocationFailed(error)
    }
}

impl Table {
    /// Scans one column against a same-typed literal and returns matching rows.
    ///
    /// Integer comparisons use signed `i64` ordering, Boolean comparisons use
    /// `false < true`, and strings use Rust's lexicographic `String` ordering.
    /// Floating-point comparisons use native IEEE 754 predicates: comparisons
    /// involving NaN are false except [`ComparisonOperator::NotEqual`], which
    /// is true, and `-0.0` compares equal to `0.0`.
    pub fn scan(
        &self,
        field: &str,
        operator: ComparisonOperator,
        literal: &Value,
    ) -> Result<RowSelection, ScanError> {
        let column_type = self
            .fields()
            .iter()
            .find(|candidate| candidate.name() == field)
            .map(|candidate| candidate.data_type())
            .ok_or_else(|| ScanError::FieldNotFound {
                name: field.to_owned(),
            })?;
        let literal_type = literal.data_type();
        if column_type != literal_type {
            return Err(ScanError::TypeMismatch {
                field: field.to_owned(),
                column_type,
                literal_type,
            });
        }

        let mut selection = RowSelection::try_empty(self.len())?;
        match (column_type, literal) {
            (DataType::Int64, Value::Int64(literal)) => {
                let values = self
                    .int64_column(field)
                    .expect("field type was validated against the literal");
                select_matches(&mut selection, values, operator, literal);
            }
            (DataType::Float64, Value::Float64(literal)) => {
                let values = self
                    .float64_column(field)
                    .expect("field type was validated against the literal");
                select_matches(&mut selection, values, operator, literal);
            }
            (DataType::Bool, Value::Bool(literal)) => {
                let values = self
                    .bool_column(field)
                    .expect("field type was validated against the literal");
                for (row, value) in values.enumerate() {
                    if operator.compare(&value, literal) {
                        selection.select(row);
                    }
                }
            }
            (DataType::String, Value::String(literal)) => {
                let values = self
                    .string_column(field)
                    .expect("field type was validated against the literal");
                select_matches(&mut selection, values, operator, literal);
            }
            _ => unreachable!("column and literal types were checked before scanning"),
        }
        Ok(selection)
    }
}

fn bitmap_byte_len(row_count: usize) -> usize {
    row_count.div_ceil(BITS_PER_BYTE)
}

fn select_matches<T: PartialEq + PartialOrd>(
    selection: &mut RowSelection,
    values: &[T],
    operator: ComparisonOperator,
    literal: &T,
) {
    for (row, value) in values.iter().enumerate() {
        if operator.compare(value, literal) {
            selection.select(row);
        }
    }
}
