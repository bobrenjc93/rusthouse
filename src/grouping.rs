//! Selection-aware grouped counts over one typed table column.
//!
//! This module is a storage-level primitive. SQL grouping, multiple grouping
//! keys, and aggregates other than count belong to the query-execution layer.

use crate::scan::RowSelection;
use crate::storage::Column;
use crate::{Table, Value};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// One distinct column value and the number of selected rows containing it.
///
/// Equality uses the same key identity as [`Table::grouped_count`], including
/// [`f64::total_cmp`] equality for floating-point values.
#[derive(Clone, Debug)]
pub struct GroupedCount {
    value: Value,
    count: usize,
}

impl GroupedCount {
    /// Returns the distinct column value identifying this group.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }

    /// Returns the number of selected rows in this group.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Splits this group into its owned value and row count.
    #[must_use]
    pub fn into_parts(self) -> (Value, usize) {
        (self.value, self.count)
    }
}

impl PartialEq for GroupedCount {
    fn eq(&self, other: &Self) -> bool {
        self.count == other.count && values_equal(&self.value, &other.value)
    }
}

impl Eq for GroupedCount {}

/// A validation, resource-limit, arithmetic, or allocation error from a
/// one-column grouped count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupedCountError {
    /// A row selection represents a different number of rows than the table.
    SelectionLengthMismatch {
        /// Number of rows in the table being grouped.
        table_rows: usize,
        /// Number of rows represented by the supplied selection.
        selection_rows: usize,
    },
    /// The requested field does not exist in the table schema.
    FieldNotFound {
        /// The requested, case-sensitive field name.
        name: String,
    },
    /// The selected values contain more distinct groups than the caller allows.
    GroupLimitExceeded {
        /// Name of the grouped field.
        field: String,
        /// Caller-supplied maximum number of groups.
        limit: usize,
    },
    /// Incrementing a group's row count would exceed the `usize` range.
    CountOverflow {
        /// Name of the grouped field.
        field: String,
    },
    /// Memory could not be reserved for the bounded group result.
    GroupAllocationFailed {
        /// Number of group entries requested from the allocator.
        requested_groups: usize,
    },
    /// Memory could not be reserved while copying a distinct String key.
    StringAllocationFailed {
        /// Name of the grouped field.
        field: String,
        /// Number of bytes requested from the allocator.
        required_bytes: usize,
    },
}

impl fmt::Display for GroupedCountError {
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
            Self::GroupLimitExceeded { field, limit } => write!(
                formatter,
                "grouped count for field `{field}` exceeds group limit {limit}"
            ),
            Self::CountOverflow { field } => {
                write!(formatter, "grouped count for field `{field}` overflowed")
            }
            Self::GroupAllocationFailed { requested_groups } => write!(
                formatter,
                "could not reserve storage for {requested_groups} grouped counts"
            ),
            Self::StringAllocationFailed {
                field,
                required_bytes,
            } => write!(
                formatter,
                "could not reserve {required_bytes} bytes for a String key of field `{field}`"
            ),
        }
    }
}

impl Error for GroupedCountError {}

impl Table {
    /// Returns the distinct values and counts of one column in ascending order.
    ///
    /// All physical column types are supported. Integers use signed ordering,
    /// Booleans use `false < true`, and strings use lexicographic Unicode scalar
    /// value ordering. `Float64` keys use [`f64::total_cmp`] for both ordering
    /// and group identity: `-0.0` and `+0.0` form separate groups, while NaN
    /// sign, payload, and signaling state participate in deterministic ordering
    /// and otherwise identical NaN bit patterns form one group.
    ///
    /// A supplied selection must represent exactly [`Table::len`] rows. Empty
    /// tables and selections return an empty result. If the selected input has
    /// more than `max_groups` distinct values, this method returns
    /// [`GroupedCountError::GroupLimitExceeded`] rather than a truncated result.
    /// Group storage and owned String keys are allocated fallibly, and counts
    /// use checked arithmetic.
    pub fn grouped_count(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
        max_groups: usize,
    ) -> Result<Vec<GroupedCount>, GroupedCountError> {
        validate_selection(self.len(), selection)?;
        let column_index = self
            .fields()
            .iter()
            .position(|candidate| candidate.name() == field)
            .ok_or_else(|| GroupedCountError::FieldNotFound {
                name: field.to_owned(),
            })?;
        let column = &self.columns()[column_index];
        let selected_count = selection.map_or_else(|| self.len(), RowSelection::selected_count);

        let mut groups = Vec::new();
        reserve_groups(&mut groups, selected_count.min(max_groups))?;
        match selection {
            Some(selection) => group_column(
                field,
                column,
                selection.selected_rows(),
                max_groups,
                &mut groups,
            )?,
            None => group_column(field, column, 0..self.len(), max_groups, &mut groups)?,
        }
        Ok(groups)
    }
}

fn validate_selection(
    table_rows: usize,
    selection: Option<&RowSelection>,
) -> Result<(), GroupedCountError> {
    if let Some(selection) = selection
        && selection.len() != table_rows
    {
        return Err(GroupedCountError::SelectionLengthMismatch {
            table_rows,
            selection_rows: selection.len(),
        });
    }
    Ok(())
}

fn reserve_groups(
    groups: &mut Vec<GroupedCount>,
    requested_groups: usize,
) -> Result<(), GroupedCountError> {
    groups
        .try_reserve_exact(requested_groups)
        .map_err(|_| GroupedCountError::GroupAllocationFailed { requested_groups })
}

fn group_column(
    field: &str,
    column: &Column,
    rows: impl Iterator<Item = usize>,
    max_groups: usize,
    groups: &mut Vec<GroupedCount>,
) -> Result<(), GroupedCountError> {
    match column {
        Column::Int64(values) => group_values(
            field,
            rows,
            max_groups,
            groups,
            |value, row| compare_value_to_i64(value, values[row]),
            |row| Ok(Value::Int64(values[row])),
        ),
        Column::Float64(values) => group_values(
            field,
            rows,
            max_groups,
            groups,
            |value, row| compare_value_to_f64(value, values[row]),
            |row| Ok(Value::Float64(values[row])),
        ),
        Column::Bool(values) => group_values(
            field,
            rows,
            max_groups,
            groups,
            |value, row| compare_value_to_bool(value, values[row] != 0),
            |row| Ok(Value::Bool(values[row] != 0)),
        ),
        Column::String(values) => group_values(
            field,
            rows,
            max_groups,
            groups,
            |value, row| compare_value_to_str(value, &values[row]),
            |row| copy_string_value(field, &values[row]),
        ),
    }
}

fn group_values(
    field: &str,
    rows: impl Iterator<Item = usize>,
    max_groups: usize,
    groups: &mut Vec<GroupedCount>,
    compare: impl Fn(&Value, usize) -> Ordering,
    make_value: impl Fn(usize) -> Result<Value, GroupedCountError>,
) -> Result<(), GroupedCountError> {
    for row in rows {
        match groups.binary_search_by(|group| compare(&group.value, row)) {
            Ok(index) => increment_count(field, &mut groups[index].count)?,
            Err(index) => {
                if groups.len() == max_groups {
                    return Err(GroupedCountError::GroupLimitExceeded {
                        field: field.to_owned(),
                        limit: max_groups,
                    });
                }
                groups.insert(
                    index,
                    GroupedCount {
                        value: make_value(row)?,
                        count: 1,
                    },
                );
            }
        }
    }
    Ok(())
}

fn increment_count(field: &str, count: &mut usize) -> Result<(), GroupedCountError> {
    *count = count
        .checked_add(1)
        .ok_or_else(|| GroupedCountError::CountOverflow {
            field: field.to_owned(),
        })?;
    Ok(())
}

fn copy_string_value(field: &str, source: &str) -> Result<Value, GroupedCountError> {
    let mut value = reserve_string(field, source.len())?;
    value.push_str(source);
    Ok(Value::String(value))
}

fn reserve_string(field: &str, required_bytes: usize) -> Result<String, GroupedCountError> {
    let mut value = String::new();
    value.try_reserve_exact(required_bytes).map_err(|_| {
        GroupedCountError::StringAllocationFailed {
            field: field.to_owned(),
            required_bytes,
        }
    })?;
    Ok(value)
}

fn compare_value_to_i64(value: &Value, other: i64) -> Ordering {
    let Value::Int64(value) = value else {
        unreachable!("a grouped result contains values from one physical column")
    };
    value.cmp(&other)
}

fn compare_value_to_f64(value: &Value, other: f64) -> Ordering {
    let Value::Float64(value) = value else {
        unreachable!("a grouped result contains values from one physical column")
    };
    value.total_cmp(&other)
}

fn compare_value_to_bool(value: &Value, other: bool) -> Ordering {
    let Value::Bool(value) = value else {
        unreachable!("a grouped result contains values from one physical column")
    };
    value.cmp(&other)
}

fn compare_value_to_str(value: &Value, other: &str) -> Ordering {
    let Value::String(value) = value else {
        unreachable!("a grouped result contains values from one physical column")
    };
    value.as_str().cmp(other)
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => left == right,
        (Value::Float64(left), Value::Float64(right)) => left.total_cmp(right).is_eq(),
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_group_capacity_allocation_errors() {
        let mut groups = Vec::new();
        assert_eq!(
            reserve_groups(&mut groups, usize::MAX),
            Err(GroupedCountError::GroupAllocationFailed {
                requested_groups: usize::MAX,
            })
        );
    }

    #[test]
    fn reports_string_key_allocation_errors() {
        assert_eq!(
            reserve_string("key", usize::MAX),
            Err(GroupedCountError::StringAllocationFailed {
                field: "key".to_owned(),
                required_bytes: usize::MAX,
            })
        );
    }

    #[test]
    fn reports_checked_count_overflow() {
        let mut count = usize::MAX;
        assert_eq!(
            increment_count("key", &mut count),
            Err(GroupedCountError::CountOverflow {
                field: "key".to_owned(),
            })
        );
        assert_eq!(count, usize::MAX);
    }
}
