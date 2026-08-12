//! Borrowed group-key indexing for the batch execution engine.
//!
//! The index owns the zero-, one-, two-, and many-column lookup shapes and
//! reconstructs retained keys in first-seen group-number order. It borrows
//! scalar payloads from immutable table storage. Query planning, resource
//! accounting, aggregate state, and SQL semantics remain engine concerns.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::batch::storage::Table;
use crate::batch::value::ValueRef;

#[derive(Debug)]
pub(super) enum GroupIndex<'a> {
    Global,
    One(HashMap<ValueRef<'a>, usize>),
    Multiple(HashMap<Box<[ValueRef<'a>]>, usize>),
}

impl<'a> GroupIndex<'a> {
    pub(super) fn new(column_count: usize) -> Self {
        match column_count {
            0 => Self::Global,
            1 => Self::One(HashMap::new()),
            _ => Self::Multiple(HashMap::new()),
        }
    }

    pub(super) fn find(
        &self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        multiple_key_probe: &mut Vec<ValueRef<'a>>,
    ) -> Option<usize> {
        match self {
            Self::Global => Some(0),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                groups.get(&key).copied()
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                groups.get(key.as_slice()).copied()
            }
            Self::Multiple(groups) => {
                multiple_key_probe.clear();
                multiple_key_probe.extend(
                    columns
                        .iter()
                        .map(|column| table.columns()[*column].value_ref(row)),
                );
                groups.get(multiple_key_probe.as_slice()).copied()
            }
        }
    }

    pub(super) fn insert(
        &mut self,
        table: &'a Table,
        columns: &[usize],
        row: usize,
        group: usize,
        multiple_key_probe: &[ValueRef<'a>],
    ) {
        let previous = match self {
            Self::Global => unreachable!("global aggregation has no grouped key to insert"),
            Self::One(groups) => {
                let key = table.columns()[columns[0]].value_ref(row);
                groups.insert(key, group)
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [
                    table.columns()[columns[0]].value_ref(row),
                    table.columns()[columns[1]].value_ref(row),
                ];
                groups.insert(key.into(), group)
            }
            Self::Multiple(groups) => {
                debug_assert_eq!(multiple_key_probe.len(), columns.len());
                groups.insert(multiple_key_probe.into(), group)
            }
        };
        debug_assert!(previous.is_none(), "new group keys must be unique");
    }

    pub(super) fn into_keys(self, group_count: usize) -> Vec<GroupKey<'a>> {
        let mut ordered = std::iter::repeat_with(|| None)
            .take(group_count)
            .collect::<Vec<_>>();
        match self {
            Self::Global => {
                debug_assert_eq!(group_count, 1);
                ordered[0] = Some(GroupKey::Empty);
            }
            Self::One(groups) => {
                for (key, group) in groups {
                    ordered[group] = Some(GroupKey::One(key));
                }
            }
            Self::Multiple(groups) => {
                for (key, group) in groups {
                    ordered[group] = Some(GroupKey::Multiple(key));
                }
            }
        }
        ordered
            .into_iter()
            .map(|key| key.expect("every group index has a key"))
            .collect()
    }
}

#[derive(Debug)]
pub(super) enum GroupKey<'a> {
    Empty,
    One(ValueRef<'a>),
    Multiple(Box<[ValueRef<'a>]>),
}

impl GroupKey<'_> {
    pub(super) fn value(&self, position: usize) -> ValueRef<'_> {
        match self {
            Self::Empty => unreachable!("a global aggregate has no grouped columns"),
            Self::One(value) if position == 0 => *value,
            Self::One(_) => unreachable!("single-column group position is zero"),
            Self::Multiple(values) => values[position],
        }
    }

    pub(super) fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Empty, Self::Empty) => Ordering::Equal,
            (Self::One(left), Self::One(right)) => left.cmp(right),
            (Self::Multiple(left), Self::Multiple(right)) => left.cmp(right),
            _ => unreachable!("all keys for a query have the same shape"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::storage::{ColumnDef, TableLimits};
    use crate::batch::value::{DataType, Value};

    fn index_rows<'a>(
        table: &'a Table,
        columns: &[usize],
        probe_capacity: usize,
    ) -> (Vec<GroupKey<'a>>, Vec<ValueRef<'a>>) {
        let mut index = GroupIndex::new(columns.len());
        let mut group_count = usize::from(columns.is_empty());
        let mut probe = Vec::with_capacity(probe_capacity);
        for row in 0..table.row_count() {
            if index.find(table, columns, row, &mut probe).is_none() {
                index.insert(table, columns, row, group_count, &probe);
                group_count += 1;
            }
        }
        (index.into_keys(group_count), probe)
    }

    #[test]
    fn global_and_single_key_indexes_preserve_identity_and_first_seen_numbers() {
        let empty_table = Table::new(
            "empty".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::Int64,
            }],
        )
        .expect("valid table");
        let (global, probe) = index_rows(&empty_table, &[], 0);
        assert!(matches!(global.as_slice(), [GroupKey::Empty]));
        assert_eq!(probe.capacity(), 0);

        let mut floats = Table::new(
            "floats".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::Float64,
            }],
        )
        .expect("valid table");
        floats
            .insert_rows(vec![
                vec![Value::Float64(5.0)],
                vec![Value::Float64(-0.0)],
                vec![Value::Float64(0.0)],
                vec![Value::Float64(5.0)],
            ])
            .expect("valid values");
        let (keys, probe) = index_rows(&floats, &[0], 0);
        assert_eq!(keys.len(), 2, "signed zero has one group identity");
        assert!(matches!(keys[0].value(0), ValueRef::Float64(5.0)));
        let ValueRef::Float64(zero) = keys[1].value(0) else {
            panic!("second first-seen key is a float")
        };
        assert_eq!(zero.to_bits(), (-0.0_f64).to_bits());
        assert_eq!(probe.capacity(), 0, "one key needs no composite probe");

        let nullable = Table::with_nullable_int64_values(
            "nullable".to_owned(),
            "value".to_owned(),
            vec![None, Some(7), None, Some(7)],
            TableLimits::default(),
        )
        .expect("valid nullable table");
        let (keys, _) = index_rows(&nullable, &[0], 0);
        assert_eq!(keys.len(), 2);
        assert!(matches!(keys[0].value(0), ValueRef::Null(DataType::Int64)));
        assert!(matches!(keys[1].value(0), ValueRef::Int64(7)));
    }

    #[test]
    fn two_and_many_key_indexes_use_borrowed_first_seen_keys() {
        let mut table = Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
                ColumnDef {
                    name: "reading".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDef {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
            ],
        )
        .expect("valid table");
        table
            .insert_rows(vec![
                vec![
                    Value::String("beta".to_owned()),
                    Value::Float64(-0.0),
                    Value::Bool(true),
                ],
                vec![
                    Value::String("alpha".to_owned()),
                    Value::Float64(1.0),
                    Value::Bool(false),
                ],
                vec![
                    Value::String("beta".to_owned()),
                    Value::Float64(0.0),
                    Value::Bool(true),
                ],
                vec![
                    Value::String("beta".to_owned()),
                    Value::Float64(-0.0),
                    Value::Bool(false),
                ],
            ])
            .expect("valid rows");

        let (two_keys, two_probe) = index_rows(&table, &[0, 1], 0);
        assert_eq!(two_keys.len(), 2);
        assert_eq!(two_probe.capacity(), 0, "two keys use a stack probe");
        assert!(matches!(two_keys[0].value(0), ValueRef::String("beta")));
        assert!(matches!(two_keys[1].value(0), ValueRef::String("alpha")));

        let (many_keys, many_probe) = index_rows(&table, &[0, 1, 2], 3);
        assert_eq!(many_keys.len(), 3);
        assert!(many_probe.capacity() >= 3);
        assert!(matches!(many_keys[0].value(2), ValueRef::Bool(true)));
        assert!(matches!(many_keys[1].value(0), ValueRef::String("alpha")));
        assert!(matches!(many_keys[2].value(2), ValueRef::Bool(false)));
        assert_eq!(many_keys[0].cmp(&many_keys[1]), Ordering::Greater);

        let ValueRef::String(indexed_label) = many_keys[0].value(0) else {
            panic!("first composite key starts with a string")
        };
        let ValueRef::String(source_label) = table.columns()[0].value_ref(0) else {
            panic!("source column is a string")
        };
        assert_eq!(indexed_label.as_ptr(), source_label.as_ptr());
    }
}
