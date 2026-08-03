//! Selection-aware grouped counts over one typed table column.
//!
//! This module is a storage-level primitive. SQL grouping, multiple grouping
//! keys, and aggregates other than count belong to the query-execution layer.

use crate::scan::RowSelection;
use crate::storage::Column;
use crate::{Table, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
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
    /// Owned distinct String keys would exceed the caller's byte limit.
    StringResultTooLarge {
        /// Name of the grouped field.
        field: String,
        /// Maximum total owned String payload bytes.
        limit: usize,
        /// Total distinct String payload bytes required by the result.
        required: usize,
    },
    /// Incrementing a group's row count would exceed the `usize` range.
    CountOverflow {
        /// Name of the grouped field.
        field: String,
    },
    /// Memory could not be reserved for grouped-count state or its result.
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
            Self::StringResultTooLarge {
                field,
                limit,
                required,
            } => write!(
                formatter,
                "grouped count for field `{field}` requires {required} owned String bytes; limit is {limit}"
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
    /// use checked arithmetic. This compatibility entry point does not limit
    /// total String key bytes; bounded callers should use
    /// [`Self::grouped_count_with_string_limit`]. Accumulation takes expected
    /// linear time in the selected row count, followed by an in-place
    /// `O(g log g)` sort for `g` distinct groups.
    pub fn grouped_count(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
        max_groups: usize,
    ) -> Result<Vec<GroupedCount>, GroupedCountError> {
        self.grouped_count_with_string_limit(field, selection, max_groups, usize::MAX)
    }

    /// Returns bounded distinct values and counts in ascending key order.
    ///
    /// This has the same grouping semantics and group-count bound as
    /// [`Self::grouped_count`]. Before copying any distinct String key, it sums
    /// their payload lengths and returns
    /// [`GroupedCountError::StringResultTooLarge`] if the result would retain
    /// more than `max_string_bytes`. Duplicate keys consume the budget once.
    pub fn grouped_count_with_string_limit(
        &self,
        field: &str,
        selection: Option<&RowSelection>,
        max_groups: usize,
        max_string_bytes: usize,
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
        let groups = match selection {
            Some(selection) => group_column(field, column, selection.selected_rows(), max_groups)?,
            None => group_column(field, column, 0..self.len(), max_groups)?,
        };
        materialize_groups(field, groups, max_string_bytes)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum GroupKey<'a> {
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(&'a str),
}

fn reserve_group_entries(
    groups: &mut HashMap<GroupKey<'_>, usize>,
    additional: usize,
    requested_groups: usize,
) -> Result<(), GroupedCountError> {
    groups
        .try_reserve(additional)
        .map_err(|_| GroupedCountError::GroupAllocationFailed { requested_groups })
}

fn reserve_group_results(
    groups: &mut Vec<GroupedCount>,
    requested_groups: usize,
) -> Result<(), GroupedCountError> {
    groups
        .try_reserve_exact(requested_groups)
        .map_err(|_| GroupedCountError::GroupAllocationFailed { requested_groups })
}

fn group_column<'a>(
    field: &str,
    column: &'a Column,
    rows: impl Iterator<Item = usize>,
    max_groups: usize,
) -> Result<HashMap<GroupKey<'a>, usize>, GroupedCountError> {
    let mut groups = HashMap::new();
    match column {
        Column::Int64(values) => group_keys(
            field,
            rows.map(|row| GroupKey::Int64(values[row])),
            max_groups,
            &mut groups,
        ),
        Column::Float64(values) => group_keys(
            field,
            rows.map(|row| GroupKey::Float64(values[row].to_bits())),
            max_groups,
            &mut groups,
        ),
        Column::Bool(values) => group_keys(
            field,
            rows.map(|row| GroupKey::Bool(values[row] != 0)),
            max_groups,
            &mut groups,
        ),
        Column::String(values) => group_keys(
            field,
            rows.map(|row| GroupKey::String(&values[row])),
            max_groups,
            &mut groups,
        ),
    }?;
    Ok(groups)
}

fn group_keys<'a>(
    field: &str,
    keys: impl Iterator<Item = GroupKey<'a>>,
    max_groups: usize,
    groups: &mut HashMap<GroupKey<'a>, usize>,
) -> Result<(), GroupedCountError> {
    for key in keys {
        if let Some(count) = groups.get_mut(&key) {
            increment_count(field, count)?;
            continue;
        }

        if groups.len() == max_groups {
            return Err(GroupedCountError::GroupLimitExceeded {
                field: field.to_owned(),
                limit: max_groups,
            });
        }
        let requested_groups =
            groups
                .len()
                .checked_add(1)
                .ok_or(GroupedCountError::GroupAllocationFailed {
                    requested_groups: usize::MAX,
                })?;
        reserve_group_entries(groups, 1, requested_groups)?;
        let previous = groups.insert(key, 1);
        debug_assert!(previous.is_none());
    }
    Ok(())
}

fn materialize_groups(
    field: &str,
    groups: HashMap<GroupKey<'_>, usize>,
    max_string_bytes: usize,
) -> Result<Vec<GroupedCount>, GroupedCountError> {
    validate_grouped_string_bytes(field, &groups, max_string_bytes)?;
    let mut result = Vec::new();
    reserve_group_results(&mut result, groups.len())?;
    for (key, count) in groups {
        result.push(GroupedCount {
            value: key.into_value(field)?,
            count,
        });
    }
    result.sort_unstable_by(|left, right| compare_values(&left.value, &right.value));
    Ok(result)
}

fn validate_grouped_string_bytes(
    field: &str,
    groups: &HashMap<GroupKey<'_>, usize>,
    limit: usize,
) -> Result<(), GroupedCountError> {
    let mut required = 0usize;
    for key in groups.keys() {
        if let GroupKey::String(value) = key {
            required = required.saturating_add(value.len());
        }
    }
    if required > limit {
        return Err(GroupedCountError::StringResultTooLarge {
            field: field.to_owned(),
            limit,
            required,
        });
    }
    Ok(())
}

impl GroupKey<'_> {
    fn into_value(self, field: &str) -> Result<Value, GroupedCountError> {
        match self {
            Self::Int64(value) => Ok(Value::Int64(value)),
            Self::Float64(bits) => Ok(Value::Float64(f64::from_bits(bits))),
            Self::Bool(value) => Ok(Value::Bool(value)),
            Self::String(value) => copy_string_value(field, value),
        }
    }
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

fn compare_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => left.cmp(right),
        (Value::Float64(left), Value::Float64(right)) => left.total_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => unreachable!("a grouped result contains values from one physical column"),
    }
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
        let mut entries: HashMap<GroupKey<'_>, usize> = HashMap::new();
        assert_eq!(
            reserve_group_entries(&mut entries, usize::MAX, usize::MAX),
            Err(GroupedCountError::GroupAllocationFailed {
                requested_groups: usize::MAX,
            })
        );

        let mut result = Vec::new();
        assert_eq!(
            reserve_group_results(&mut result, usize::MAX),
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
